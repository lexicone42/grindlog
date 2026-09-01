#!/usr/bin/env bash
# Build the records site from the live database and push it to
# https://ng.lexicone.com (S3 + CloudFront, stack: infra/site-stack.yml).
#
#   ./scripts/deploy-site.sh              # build + upload + invalidate
#   ./scripts/deploy-site.sh --infra      # also create/update the CFN stack
#
# The post-stream ritual is just: ./scripts/deploy-site.sh
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

./scripts/build-site.sh

echo "--- uploading ---"
aws s3 cp site/index.html "s3://${DOMAIN}/index.html" \
  --region "$REGION" \
  --content-type "text/html; charset=utf-8" \
  --cache-control "public, max-age=60"

DIST_ID=$(aws cloudformation describe-stacks --region "$REGION" --stack-name "$STACK" \
  --query "Stacks[0].Outputs[?OutputKey=='DistributionId'].OutputValue" --output text)
aws cloudfront create-invalidation --distribution-id "$DIST_ID" --paths "/index.html" "/" \
  --query 'Invalidation.Id' --output text

echo "live: https://${DOMAIN}/"
