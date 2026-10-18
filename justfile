# costerra — run `just` to list recipes.

set dotenv-load := true

bin := "target/release/costerra"
image := env("COSTERRA_IMAGE", "costerra:dev")

# List recipes
default:
    @just --list --unsorted

# Install the toolchain (mise) and fetch dependencies
setup:
    mise install
    cargo fetch

# Debug build (lean profile: see [profile.dev] in Cargo.toml)
build:
    cargo build

# Optimized build
release:
    cargo build --release

# Run costerra with arguments, e.g. `just run breakdown ../infra --format json`
run *args: release
    {{ bin }} {{ args }}

# Cost breakdown of a path (default: current directory)
breakdown path="." *args: release
    {{ bin }} breakdown {{ path }} {{ args }}

# Cost diff of a path against a git ref
diff path="." base="origin/main" *args: release
    {{ bin }} diff {{ path }} --base {{ base }} {{ args }}

# Breakdown of all test fixtures
fixtures *args: release
    {{ bin }} breakdown testdata --show-all {{ args }}

# Unit, golden, CLI and fixture tests (dev profile: shares artifacts with clippy, no LTO builds)
test:
    cargo test

# Format the code
fmt:
    cargo fmt

# Check formatting and lints (warnings are errors)
lint:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

# Everything CI runs
check: lint test

# Licences, advisories and sources of the dependencies (deny.toml)
deny:
    cargo-deny check

# Vulnerability and secret scans: dependencies (cargo-deny, osv-scanner), repository (trivy, gitleaks)
audit: deny
    osv-scanner scan source --recursive .
    gitleaks dir . --no-banner --redact
    trivy fs --quiet --scanners vuln,secret --exit-code 1 .
    # testdata/ holds deliberately unhardened Terraform fixtures: no misconfiguration checks there.
    trivy fs --quiet --scanners misconfig --skip-dirs testdata --exit-code 1 .

# Rebuild the golden price list of the tests from the synced catalog (review the diff)
golden-catalog:
    cargo test --test golden regenerate_golden_catalog -- --ignored --nocapture

# Download prices; with a billing account, also its contract prices
sync-prices billing_account="": release
    {{ bin }} pricing sync {{ if billing_account != "" { "--billing-account " + billing_account } else { "" } }}

# Pricing catalog information
pricing-status: release
    {{ bin }} pricing status

# Search SKUs, e.g. `just sku '^N2 Instance Core' 'Compute Engine' europe-west1`
sku query service="" region="": release
    {{ bin }} pricing search '{{ query }}' --service '{{ service }}' --region '{{ region }}'

# List supported resource types and their usage keys
resources: release
    {{ bin }} resources

# Benchmark a breakdown (prices cached, no network): time and peak memory
bench path="testdata": release
    hyperfine --warmup 2 '{{ bin }} breakdown {{ path }} --offline --list-prices'

# Profile the CPU of a breakdown with samply (from mise.toml); opens the Firefox Profiler
profile path="testdata" *args="--offline --list-prices":
    cargo build --profile profiling
    samply record target/profiling/costerra breakdown {{ path }} {{ args }}

# Peak memory (max resident set size) of a breakdown
mem path="testdata" *args="--offline --list-prices":
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --profile profiling -q
    if [ "$(uname)" = Darwin ]; then
      /usr/bin/time -l target/profiling/costerra breakdown {{ path }} {{ args }} 2>&1 >/dev/null | awk '/maximum resident/ {printf "peak memory: %.1f MiB\n", $1 / 1048576}'
    else
      /usr/bin/time -v target/profiling/costerra breakdown {{ path }} {{ args }} 2>&1 >/dev/null | awk -F: '/Maximum resident/ {printf "peak memory: %.1f MiB\n", $2 / 1024}'
    fi

# Generate a plan JSON fixture from testdata/terraform/basic (needs GCP credentials)
plan-fixture out="/tmp/costerra-plan.json":
    #!/usr/bin/env bash
    set -euo pipefail
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    cp -r testdata/terraform/basic/. "$work"
    sed -i '/backend "gcs"/d' "$work/main.tf"
    terraform -chdir="$work" init -input=false >/dev/null
    terraform -chdir="$work" plan -refresh=false -input=false -out=plan.out >/dev/null
    terraform -chdir="$work" show -json plan.out > {{ out }}
    echo "wrote {{ out }}"

# Build the container image (cargo caches stay in BuildKit cache mounts, not in layers)
docker-build tag=image:
    DOCKER_BUILDKIT=1 docker build -t {{ tag }} .

# Run the container on a path, e.g. `just docker-run breakdown . --offline`
docker-run *args:
    docker run --rm -it -v "$PWD:/work" -v "${COSTERRA_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/costerra}:/home/nonroot/.cache/costerra" {{ image }} {{ args }}

# Vulnerability scan of the container image
docker-scan tag=image:
    trivy image --quiet --scanners vuln --exit-code 1 {{ tag }}

# Install costerra into ~/.cargo/bin
install:
    cargo install --locked --path .

# Disk usage of build artifacts and caches
disk:
    @du -sh target target/* 2>/dev/null || true
    @du -sh "${CARGO_HOME:-$HOME/.cargo}/registry" 2>/dev/null || true
    @docker system df 2>/dev/null | grep -i 'build cache' || true

# Free disk space: dev/test artifacts, release intermediates (keeps the binary) and Docker BuildKit cache mounts
sweep:
    cargo clean --profile dev
    rm -rf target/release/incremental target/release/.fingerprint target/release/build target/release/deps target/tmp
    -docker builder prune -f --filter type=exec.cachemount >/dev/null 2>&1
    @just disk

# Remove costerra's cache (prices, billing accounts, modules, recommendations)
[confirm("Delete the costerra cache directory?")]
clear-cache:
    rm -rf "${COSTERRA_CACHE_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/costerra}"

# Remove all build artifacts
clean:
    cargo clean
