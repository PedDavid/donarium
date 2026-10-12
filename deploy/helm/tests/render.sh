#!/usr/bin/env bash
# Checks the rendered chart for contracts that `helm lint` does not see.
# Needs helm and yq (mikefarah, v4).
set -euo pipefail
cd "$(dirname "$0")/../../.."
chart=deploy/helm/donarium
render() {
  helm template donarium "$chart" --set oidc.issuer=https://idp.example.com "$@"
}
fail() { echo "FAIL: $*" >&2; exit 1; }

# An exposed UI Service never carries the unauthenticated metrics port.
out=$(render --set service.type=LoadBalancer --set serviceMonitor.enabled=true)
ui=$(yq 'select(.kind == "Service" and .metadata.name == "donarium")' <<<"$out")
[[ $(yq '.spec.type' <<<"$ui") == LoadBalancer ]] || fail "UI Service type not applied"
[[ $(yq '[.spec.ports[].name] | join(",")' <<<"$ui") == http ]] || fail "UI Service exposes more than http"
metrics=$(yq 'select(.kind == "Service" and .metadata.name == "donarium-metrics")' <<<"$out")
[[ $(yq '.spec.type' <<<"$metrics") == ClusterIP ]] || fail "metrics Service is not ClusterIP"
[[ $(yq '.spec.ports[0].name' <<<"$metrics") == metrics ]] || fail "metrics Service has no metrics port"
sm=$(yq 'select(.kind == "ServiceMonitor")' <<<"$out")
[[ $(yq '.spec.selector.matchLabels."app.kubernetes.io/component"' <<<"$sm") == metrics ]] \
  || fail "ServiceMonitor does not select the metrics Service"

# The database volume: a claim that survives uninstall by default, the
# caller's own claim when given, and an emptyDir only when switched off.
out=$(render)
pvc=$(yq 'select(.kind == "PersistentVolumeClaim")' <<<"$out")
[[ $(yq '.metadata.annotations."helm.sh/resource-policy"' <<<"$pvc") == keep ]] \
  || fail "the data claim is not kept on uninstall"
claim=$(yq 'select(.kind == "Deployment") | .spec.template.spec.volumes[] | select(.name == "data") | .persistentVolumeClaim.claimName' <<<"$out")
[[ $claim == "$(yq '.metadata.name' <<<"$pvc")" ]] || fail "the Deployment does not mount the data claim"
out=$(render --set persistence.existingClaim=mine)
[[ -z $(yq 'select(.kind == "PersistentVolumeClaim")' <<<"$out") ]] || fail "a claim was created despite existingClaim"
[[ $(yq 'select(.kind == "Deployment") | .spec.template.spec.volumes[] | select(.name == "data") | .persistentVolumeClaim.claimName' <<<"$out") == mine ]] \
  || fail "existingClaim is not mounted"
out=$(render --set persistence.enabled=false)
[[ $(yq 'select(.kind == "Deployment") | .spec.template.spec.volumes[] | select(.name == "data") | has("emptyDir")' <<<"$out") == true ]] \
  || fail "persistence.enabled=false does not use an emptyDir"

# The default image tag is the chart's appVersion, which the release
# workflow publishes (see .github/workflows/release.yml).
app=$(yq '.appVersion' "$chart/Chart.yaml")
image=$(render | yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].image')
[[ $image == *":$app" ]] || fail "default image $image is not tagged $app"
cargo_version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[[ $app == "$cargo_version" ]] || fail "Chart appVersion $app differs from Cargo.toml version $cargo_version"

echo "chart render checks passed"
