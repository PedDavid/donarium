# Tailwind v4 standalone CLI (no Node.js needed):
# https://github.com/tailwindlabs/tailwindcss/releases/tag/v4.3.3
TAILWIND ?= tailwindcss

.PHONY: css check demo helm-lint

css: ## Rebuild assets/dist/app.css from assets/app.css and the templates
	$(TAILWIND) -i assets/app.css -o assets/dist/app.css

check: ## What CI runs
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo test --locked

demo: ## Run the UI with sample data and no auth
	cargo run -- --demo

helm-lint: ## Lint and render the chart
	helm lint deploy/helm/donarium --set oidc.issuer=https://idp.example.com
	helm template donarium deploy/helm/donarium --set oidc.issuer=https://idp.example.com \
	  --set serviceMonitor.enabled=true >/dev/null
	deploy/helm/tests/render.sh
