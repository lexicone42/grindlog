#!/usr/bin/env bash
# Build the records site from the live database and push it to
# https://ng.lexicone.com (S3 + CloudFront, stack: infra/site-stack.yml).
#
#   ./scripts/deploy-site.sh              # fill run numbers + build + upload + invalidate
#   ./scripts/deploy-site.sh --infra      # also update the existing CFN stack
#
# --infra passes no parameters, so it can only update a stack that already
# exists (CloudFormation reuses the previous values). Create it once by hand
# with your Route53 zone id, which the template does not default:
#   aws cloudformation deploy --region us-east-1 --stack-name grindlog-site \
#     --template-file infra/site-stack.yml --parameter-overrides HostedZoneId=Z...
#
# It writes to the live database first: fill-run-numbers.sh fills in
# ls_attempt numbers inferred from neighbouring runs. Normally cron runs this
# for you: deploy-if-live.sh (every 10 minutes, only while a live session is
# open) execs it, and a nightly entry runs it directly; the schedule lives in
# the crontab, not in the repo. import-when-done.sh and import-vod.sh
# --deploy call it too. Run it by hand to publish right away, after an import
# or a hand edit of the database.
set -euo pipefail
cd "$(dirname "$0")/.."

REGION=us-east-1
STACK=grindlog-site
DOMAIN=ng.lexicone.com

if [ "${1:-}" = "--infra" ]; then
  echo "--- deploying CloudFormation stack ${STACK} ---"
  aws cloudformation deploy \
    --region "$REGION" \
    --stack-name "$STACK" \
    --template-file infra/site-stack.yml \
    --no-fail-on-empty-changeset
fi

# Fill in run numbers that can be inferred from their neighbors first.
./scripts/fill-run-numbers.sh ninja-gaiden.db | tail -1
./scripts/build-site.sh

echo "--- uploading ---"
aws s3 cp site/index.html "s3://${DOMAIN}/index.html" \
  --region "$REGION" \
  --content-type "text/html; charset=utf-8" \
  --cache-control "public, max-age=60"
# The machine-readable data (build-site.sh writes site/api/v1/) and the static
# documents that describe it. They are not invalidated: their max-age is what
# governs freshness at the edge, and a minute is the promise made in the docs.
# (The one exception, a changed closed day of the per-day feed, is below.)
for f in latest summary report index; do
  aws s3 cp "site/api/v1/$f.json" "s3://${DOMAIN}/api/v1/$f.json" --region "$REGION" --only-show-errors \
    --content-type "application/json; charset=utf-8" --cache-control "public, max-age=60"
done
# The per-day feed (`report --api-dir`, src/api.rs), driven by its manifest so
# a stale local file is never uploaded. A closed day is served as immutable:
# its bytes only change when its database rows do, and the manifest's sha256
# says when. Rows of a closed day do change now and then, though (a VOD
# import replaces the whole day and renumbers attempt_number across the
# database), and an immutable object at the edge would then be wrong for a
# year. So a closed day that was already published as closed with other bytes
# gets the days/ prefix invalidated below (one wildcard path, whatever the
# count), and one already published with these very bytes is not re-uploaded.
manifest=site/api/v1/manifest.json
prev=$(aws s3 cp "s3://${DOMAIN}/api/v1/manifest.json" - --region "$REGION" --only-show-errors 2>/dev/null || true)
jq -e .days <<<"$prev" >/dev/null 2>&1 || prev='{"days":[]}'
days_changed=0
while IFS=$'\t' read -r day path closed sha; do
  # The sha256 this day was last published with, if it was published closed.
  old=$(jq -r --arg d "$day" '[.days[] | select(.day == $d and .closed == true) | .sha256] | first // empty' <<<"$prev")
  if [ "$closed" = true ]; then
    [ "$old" = "$sha" ] && continue
    cc="public, max-age=31536000, immutable"
  else
    cc="public, max-age=60"
  fi
  if [ -n "$old" ]; then echo "closed day $day changed since it was published"; days_changed=1; fi
  aws s3 cp "site/api/v1/$path" "s3://${DOMAIN}/api/v1/$path" --region "$REGION" --only-show-errors \
    --content-type "application/json; charset=utf-8" --cache-control "$cc"
done < <(jq -r '.days[] | [.day, .path, .closed, .sha256] | @tsv' "$manifest")
aws s3 cp site/api/v1/history.json "s3://${DOMAIN}/api/v1/history.json" --region "$REGION" --only-show-errors \
  --content-type "application/json; charset=utf-8" --cache-control "public, max-age=60"
aws s3 cp site/api/v1/schema.json "s3://${DOMAIN}/api/v1/schema.json" --region "$REGION" --only-show-errors \
  --content-type "application/json; charset=utf-8" --cache-control "public, max-age=3600"
# The manifest goes last, once everything it points at is in place.
aws s3 cp "$manifest" "s3://${DOMAIN}/api/v1/manifest.json" --region "$REGION" --only-show-errors \
  --content-type "application/json; charset=utf-8" --cache-control "public, max-age=60"
aws s3 cp site/static/api/v1/README.md "s3://${DOMAIN}/api/v1/README.md" --region "$REGION" --only-show-errors \
  --content-type "text/markdown; charset=utf-8" --cache-control "public, max-age=3600"
aws s3 cp site/static/api/index.json "s3://${DOMAIN}/api/index.json" --region "$REGION" --only-show-errors \
  --content-type "application/json; charset=utf-8" --cache-control "public, max-age=3600"
aws s3 cp site/static/llms.txt "s3://${DOMAIN}/llms.txt" --region "$REGION" --only-show-errors \
  --content-type "text/plain; charset=utf-8" --cache-control "public, max-age=3600"
aws s3 cp site/static/404.html "s3://${DOMAIN}/404.html" --region "$REGION" --only-show-errors \
  --content-type "text/html; charset=utf-8" --cache-control "public, max-age=3600"

DIST_ID=$(aws cloudformation describe-stacks --region "$REGION" --stack-name "$STACK" \
  --query "Stacks[0].Outputs[?OutputKey=='DistributionId'].OutputValue" --output text)
# The feed's other files are never invalidated (their max-age is the promise
# made in the docs); the immutable day files are, and only when one changed.
paths=("/index.html" "/")
[ "$days_changed" = 1 ] && paths+=("/api/v1/days/*")
aws cloudfront create-invalidation --distribution-id "$DIST_ID" --paths "${paths[@]}" \
  --query 'Invalidation.Id' --output text

echo "live: https://${DOMAIN}/"
