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

# Image scans: the GitLab token is mounted and passed as a file only when a
# Secret is named, and the interval and extra registries reach the container.
out=$(render --set gitlab.tokenSecret.existingSecret=gl --set scanInterval=6h --set registryAllow='{reg.example.com,other.example.org}')
env=$(yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].env' <<<"$out")
[[ $(yq '.[] | select(.name == "DONARIUM_GITLAB_TOKEN_FILE") | .value' <<<"$env") == /etc/donarium/gitlab/token ]] \
  || fail "the GitLab token file is not passed"
[[ $(yq '.[] | select(.name == "DONARIUM_SCAN_INTERVAL") | .value' <<<"$env") == 6h ]] || fail "scanInterval is not passed"
[[ $(yq '.[] | select(.name == "DONARIUM_REGISTRY_ALLOW") | .value' <<<"$env") == reg.example.com,other.example.org ]] \
  || fail "registryAllow is not passed"
[[ $(yq 'select(.kind == "Deployment") | .spec.template.spec.volumes[] | select(.name == "gitlab-token") | .secret.secretName' <<<"$out") == gl ]] \
  || fail "the GitLab token Secret is not mounted"
env=$(render | yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].env')
[[ -z $(yq '.[] | select(.name == "DONARIUM_GITLAB_TOKEN_FILE")' <<<"$env") ]] || fail "a token file is passed without a Secret"
[[ $(yq '.[] | select(.name == "DONARIUM_GITLAB_URL") | .value' <<<"$env") == https://gitlab.com ]] || fail "the GitLab URL default is wrong"

# Gitea/Forgejo and GitHub: the server URL and each token file are passed (and
# the Secret mounted) only when configured, and a token Secret without a Gitea
# URL is ignored rather than mounted for nothing.
out=$(render --set gitea.url=https://git.example.com --set gitea.tokenSecret.existingSecret=gt --set github.tokenSecret.existingSecret=gh)
env=$(yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].env' <<<"$out")
[[ $(yq '.[] | select(.name == "DONARIUM_GITEA_URL") | .value' <<<"$env") == https://git.example.com ]] || fail "the Gitea URL is not passed"
[[ $(yq '.[] | select(.name == "DONARIUM_GITEA_TOKEN_FILE") | .value' <<<"$env") == /etc/donarium/gitea/token ]] || fail "the Gitea token file is not passed"
[[ $(yq '.[] | select(.name == "DONARIUM_GITHUB_TOKEN_FILE") | .value' <<<"$env") == /etc/donarium/github/token ]] || fail "the GitHub token file is not passed"
for name in gitea-token:gt github-token:gh; do
  [[ $(yq "select(.kind == \"Deployment\") | .spec.template.spec.volumes[] | select(.name == \"${name%%:*}\") | .secret.secretName" <<<"$out") == "${name##*:}" ]] \
    || fail "the ${name%%:*} Secret is not mounted"
done
out=$(render --set gitea.tokenSecret.existingSecret=gt)
env=$(yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].env' <<<"$out")
[[ -z $(yq '.[] | select(.name == "DONARIUM_GITEA_URL" or .name == "DONARIUM_GITEA_TOKEN_FILE")' <<<"$env") ]] \
  || fail "Gitea is configured without a URL"
[[ -z $(yq 'select(.kind == "Deployment") | .spec.template.spec.volumes[] | select(.name == "gitea-token")' <<<"$out") ]] \
  || fail "a Gitea token Secret is mounted without a URL"

# Registry logins are passed and mounted only when a Secret is named.
out=$(render --set registryCredentials.existingSecret=regcred)
env=$(yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].env' <<<"$out")
[[ $(yq '.[] | select(.name == "DONARIUM_REGISTRY_CREDENTIALS_FILE") | .value' <<<"$env") == /etc/donarium/registry/.dockerconfigjson ]] || fail "the registry credentials file is not passed"
[[ $(yq 'select(.kind == "Deployment") | .spec.template.spec.volumes[] | select(.name == "registry-credentials") | .secret.secretName' <<<"$out") == regcred ]] || fail "the registry Secret is not mounted"
[[ -z $(render | yq 'select(.kind == "Deployment") | .spec.template.spec.volumes[] | select(.name == "registry-credentials")') ]] || fail "registry credentials mounted without a Secret"

# The base currency reaches the container, EUR unless set.
env=$(render | yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].env')
[[ $(yq '.[] | select(.name == "DONARIUM_BASE_CURRENCY") | .value' <<<"$env") == EUR ]] || fail "the base currency default is not EUR"
env=$(render --set baseCurrency=USD | yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].env')
[[ $(yq '.[] | select(.name == "DONARIUM_BASE_CURRENCY") | .value' <<<"$env") == USD ]] || fail "baseCurrency is not passed"

# Exchange rates: the service URL is always passed, and they can be turned off.
env=$(render | yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].env')
[[ $(yq '.[] | select(.name == "DONARIUM_FX_URL") | .value' <<<"$env") == https://api.frankfurter.dev ]] || fail "the exchange rate URL default is wrong"
[[ -z $(yq '.[] | select(.name == "DONARIUM_FX_DISABLED")' <<<"$env") ]] || fail "rates are disabled by default"
env=$(render --set fx.disabled=true --set fx.url=https://rates.example.com | yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].env')
[[ $(yq '.[] | select(.name == "DONARIUM_FX_DISABLED") | .value' <<<"$env") == true ]] || fail "fx.disabled is not passed"
[[ $(yq '.[] | select(.name == "DONARIUM_FX_URL") | .value' <<<"$env") == https://rates.example.com ]] || fail "fx.url is not passed"

# The default image tag is the chart's appVersion, which the release
# workflow publishes (see .github/workflows/release.yml).
app=$(yq '.appVersion' "$chart/Chart.yaml")
image=$(render | yq 'select(.kind == "Deployment") | .spec.template.spec.containers[0].image')
[[ $image == *":$app" ]] || fail "default image $image is not tagged $app"
cargo_version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[[ $app == "$cargo_version" ]] || fail "Chart appVersion $app differs from Cargo.toml version $cargo_version"

echo "chart render checks passed"
