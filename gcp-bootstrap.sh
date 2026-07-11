#!/usr/bin/env bash
# One-time Google Cloud setup for deploying this app to Cloud Run from GitHub
# Actions via keyless Workload Identity Federation (WIF).
#
# Run this ONCE, authenticated as yourself (a project Owner/Editor):
#   gcloud auth login
#   ./gcp-bootstrap.sh
#
# It is idempotent — safe to re-run. At the end it prints the two values to
# paste into your GitHub repo secrets (Settings -> Secrets and variables ->
# Actions): WIF_PROVIDER and DEPLOY_SA_EMAIL.
set -euo pipefail

PROJECT_ID="youtube-downloader-501718"
REGION="us-central1"
AR_REPO="apps"
GITHUB_REPO="Vhovsepyan/youtube-downloader"   # owner/repo allowed to deploy

POOL="github-pool"
PROVIDER="github-provider"
DEPLOY_SA="gh-deployer"

echo "==> Using project $PROJECT_ID"
gcloud config set project "$PROJECT_ID" >/dev/null

PROJECT_NUMBER="$(gcloud projects describe "$PROJECT_ID" --format='value(projectNumber)')"
DEPLOY_SA_EMAIL="${DEPLOY_SA}@${PROJECT_ID}.iam.gserviceaccount.com"
# Cloud Run runs as the default compute service account unless told otherwise.
RUNTIME_SA_EMAIL="${PROJECT_NUMBER}-compute@developer.gserviceaccount.com"

echo "==> Enabling required APIs..."
gcloud services enable \
  run.googleapis.com \
  artifactregistry.googleapis.com \
  iamcredentials.googleapis.com \
  secretmanager.googleapis.com \
  --project "$PROJECT_ID"

echo "==> Ensuring Artifact Registry repo '$AR_REPO' ($REGION)..."
if ! gcloud artifacts repositories describe "$AR_REPO" \
      --project "$PROJECT_ID" --location "$REGION" >/dev/null 2>&1; then
  gcloud artifacts repositories create "$AR_REPO" \
    --project "$PROJECT_ID" --location "$REGION" \
    --repository-format=docker \
    --description="Container images for $GITHUB_REPO"
fi

echo "==> Ensuring AUTH_TOKEN secret in Secret Manager..."
if ! gcloud secrets describe AUTH_TOKEN --project "$PROJECT_ID" >/dev/null 2>&1; then
  # printf '%s' (no trailing newline) so the stored secret is exactly the
  # token — a trailing newline would never match what a user pastes into the UI.
  printf '%s' "$(openssl rand -hex 32)" | gcloud secrets create AUTH_TOKEN \
    --project "$PROJECT_ID" --replication-policy=automatic --data-file=-
  echo "    Created AUTH_TOKEN. Read it (share with friends) via:"
  echo "      gcloud secrets versions access latest --secret=AUTH_TOKEN --project=$PROJECT_ID"
else
  echo "    AUTH_TOKEN already exists — leaving it untouched."
fi

echo "==> Ensuring deploy service account '$DEPLOY_SA'..."
if ! gcloud iam service-accounts describe "$DEPLOY_SA_EMAIL" \
      --project "$PROJECT_ID" >/dev/null 2>&1; then
  gcloud iam service-accounts create "$DEPLOY_SA" \
    --project "$PROJECT_ID" \
    --display-name="GitHub Actions deployer"
fi

echo "==> Granting deploy permissions..."
# Push images, deploy Cloud Run services.
gcloud projects add-iam-policy-binding "$PROJECT_ID" \
  --member="serviceAccount:${DEPLOY_SA_EMAIL}" \
  --role="roles/artifactregistry.writer" --condition=None >/dev/null
gcloud projects add-iam-policy-binding "$PROJECT_ID" \
  --member="serviceAccount:${DEPLOY_SA_EMAIL}" \
  --role="roles/run.admin" --condition=None >/dev/null
# Let the deployer deploy a service that RUNS AS the runtime SA.
gcloud iam service-accounts add-iam-policy-binding "$RUNTIME_SA_EMAIL" \
  --project "$PROJECT_ID" \
  --member="serviceAccount:${DEPLOY_SA_EMAIL}" \
  --role="roles/iam.serviceAccountUser" >/dev/null
# Let the running service read the AUTH_TOKEN secret.
gcloud secrets add-iam-policy-binding AUTH_TOKEN \
  --project "$PROJECT_ID" \
  --member="serviceAccount:${RUNTIME_SA_EMAIL}" \
  --role="roles/secretmanager.secretAccessor" >/dev/null
# Same for the youtube-cookies secret, but only if you've created it (see
# README/cookies setup). The deploy mounts it to get past YouTube's bot check.
if gcloud secrets describe youtube-cookies --project "$PROJECT_ID" >/dev/null 2>&1; then
  gcloud secrets add-iam-policy-binding youtube-cookies \
    --project "$PROJECT_ID" \
    --member="serviceAccount:${RUNTIME_SA_EMAIL}" \
    --role="roles/secretmanager.secretAccessor" >/dev/null
else
  echo "    (youtube-cookies secret not found yet — create it to enable cookie auth)"
fi

echo "==> Ensuring Workload Identity pool + provider..."
if ! gcloud iam workload-identity-pools describe "$POOL" \
      --project "$PROJECT_ID" --location=global >/dev/null 2>&1; then
  gcloud iam workload-identity-pools create "$POOL" \
    --project "$PROJECT_ID" --location=global \
    --display-name="GitHub Actions pool"
fi

if ! gcloud iam workload-identity-pools providers describe "$PROVIDER" \
      --project "$PROJECT_ID" --location=global \
      --workload-identity-pool="$POOL" >/dev/null 2>&1; then
  # attribute-condition restricts this provider to exactly our repo, so no
  # other GitHub repo can impersonate the deployer SA.
  gcloud iam workload-identity-pools providers create-oidc "$PROVIDER" \
    --project "$PROJECT_ID" --location=global \
    --workload-identity-pool="$POOL" \
    --display-name="GitHub provider" \
    --issuer-uri="https://token.actions.githubusercontent.com" \
    --attribute-mapping="google.subject=assertion.sub,attribute.repository=assertion.repository" \
    --attribute-condition="assertion.repository=='${GITHUB_REPO}'"
fi

echo "==> Allowing $GITHUB_REPO to impersonate the deployer SA..."
gcloud iam service-accounts add-iam-policy-binding "$DEPLOY_SA_EMAIL" \
  --project "$PROJECT_ID" \
  --role="roles/iam.workloadIdentityUser" \
  --member="principalSet://iam.googleapis.com/projects/${PROJECT_NUMBER}/locations/global/workloadIdentityPools/${POOL}/attribute.repository/${GITHUB_REPO}" >/dev/null

WIF_PROVIDER="projects/${PROJECT_NUMBER}/locations/global/workloadIdentityPools/${POOL}/providers/${PROVIDER}"

cat <<EOF

============================================================================
Done. Add these two GitHub repo secrets
(Settings -> Secrets and variables -> Actions -> New repository secret):

  WIF_PROVIDER      $WIF_PROVIDER
  DEPLOY_SA_EMAIL   $DEPLOY_SA_EMAIL

Then push to master (or run the "Deploy to Cloud Run" workflow manually) and
the app deploys automatically. Get its URL any time with:

  gcloud run services describe youtube-downloader \\
    --project $PROJECT_ID --region $REGION --format='value(status.url)'

Share that URL plus the AUTH_TOKEN value with your friends.
============================================================================
EOF
