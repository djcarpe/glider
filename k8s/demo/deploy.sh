#!/usr/bin/env bash
# Build the image, push it, and roll the demo out.
#
#   k8s/demo/deploy.sh                # build + push + apply everything
#   k8s/demo/deploy.sh apply          # apply only (image already pushed)
#   k8s/demo/deploy.sh bench          # run the benchmark job again
#   k8s/demo/deploy.sh down           # delete the namespace (and the data)
#
# Environment: IMAGE (default harbor.home/library/glider:<git sha>),
# NODE (default k3s-4), SIZES (default "1g 5g").
set -euo pipefail
cd "$(dirname "$0")/../.."

# A dirty tree gets a tag that changes per build, so a rollout always picks
# up the new image.
SHA=$(git rev-parse --short=8 HEAD)$(git diff --quiet HEAD -- . ':!k8s' || echo "-dirty-$(date +%H%M%S)")
IMAGE=${IMAGE:-harbor.home/library/glider:$SHA}
NODE=${NODE:-k3s-4}
SIZES=${SIZES:-"1g 5g"}
NS=glider-demo

bytes_of() { case "$1" in 1g) echo 1GiB;; 5g) echo 5GiB;; 10g) echo 10GiB;; *) echo "${1%g}GiB";; esac; }

render() { # file, extra sed exprs...
  local f=$1; shift
  sed -e "s|__IMAGE__|$IMAGE|g" -e "s|__NODE__|$NODE|g" "$@" "$f"
}

build() {
  echo "== building $IMAGE"
  docker build -t "$IMAGE" .
  docker push "$IMAGE"
}

apply() {
  echo "== applying to $NS on $NODE with $IMAGE"
  kubectl apply -f k8s/demo/00-base.yaml
  for s in $SIZES; do
    # A Job's template is immutable; a finished generator is recreated and
    # exits at once because the graph is already there.
    kubectl delete job -n $NS "glider-gen-$s" --ignore-not-found --wait=true
    render k8s/demo/stack.yaml.tmpl -e "s|__SIZE__|$s|g" -e "s|__BYTES__|$(bytes_of "$s")|g" | kubectl apply -f -
  done
}

bench() { # [--algos] [--writes] [--all]: the whole-graph algorithms (slow out of core), the write phase, or both
  local run extra=""; run=$(date +%Y%m%d-%H%M%S)
  # extra args as sed replacement text: `\n` is a newline to GNU sed
  case "${1:-}" in
    --algos) extra='- --algos' ;;
    --writes) extra='- --writes' ;;
    --all) extra='- --algos\n            - --writes' ;;
  esac
  echo "== benchmark job glider-bench-$run"
  render k8s/demo/40-bench.yaml.tmpl -e "s|__RUN__|$run|g" -e "s|__EXTRA__|$extra|g" | kubectl apply -f -
  echo "follow with: kubectl logs -n $NS job/glider-bench-$run -f"
}

case "${1:-all}" in
  all) build; apply; bench "${2:-}" ;;
  build) build ;;
  apply) apply ;;
  bench) bench "${2:-}" ;;
  down) kubectl delete namespace $NS ;;
  *) echo "usage: $0 [all|build|apply|bench|down]" >&2; exit 2 ;;
esac
