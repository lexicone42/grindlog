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
for f in latest summary report index; do
  aws s3 cp "site/api/v1/$f.json" "s3://${DOMAIN}/api/v1/$f.json" --region "$REGION" --only-show-errors \
    --content-type "application/json; charset=utf-8" --cache-control "public, max-age=60"
done
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
aws cloudfront create-invalidation --distribution-id "$DIST_ID" --paths "/index.html" "/" \
  --query 'Invalidation.Id' --output text

echo "live: https://${DOMAIN}/"
