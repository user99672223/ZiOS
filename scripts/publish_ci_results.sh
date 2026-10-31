#!/usr/bin/env bash
# Publishes Simulator screenshots and logs to the `ci-results` branch so they can
# be inspected with plain git (artifact downloads need extra network access).
# The branch holds only the latest run: each publish is a fresh orphan commit.
#
#   scripts/publish_ci_results.sh <results directory>
set -euo pipefail

results="$1"
: "${GITHUB_TOKEN:?GITHUB_TOKEN is required}"
: "${GITHUB_REPOSITORY:?}"
: "${GITHUB_SHA:?}"
: "${GITHUB_RUN_ID:?}"

staging="$(mktemp -d)"
cp -R "$results"/. "$staging/" 2>/dev/null || true
# Keep the branch small: drop anything over 5 MB.
find "$staging" -type f -size +5M -print -delete
cat > "$staging/RUN.md" <<INFO
Commit: $GITHUB_SHA
Run: https://github.com/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID
Job status: ${JOB_STATUS:-unknown}
INFO

cd "$staging"
git init -q
branch="${RESULTS_BRANCH:-ci-results}"
git checkout -q -b "$branch"
git add -A
git -c user.name="ZiOS CI" -c user.email="ci@users.noreply.github.com" \
    commit -q -m "Simulator results for $GITHUB_SHA (run $GITHUB_RUN_ID)"
for attempt in 1 2 3; do
    if git push -q -f "https://x-access-token:${GITHUB_TOKEN}@github.com/${GITHUB_REPOSITORY}.git" "$branch"; then
        echo "published results to the $branch branch"
        exit 0
    fi
    sleep $((attempt * 5))
done
echo "could not publish results" >&2
exit 1
