#!/usr/bin/env bash
# Authenticate pulls from the ECR Public mirror (ci/registries.toml) so CI is not
# subject to the shared anonymous quota ("OCI API error: Data limit exceeded", #583).
#
# Pelagos resolves credentials per registry host from $HOME/.docker/config.json
# (src/cli/auth.rs), so we only need to write auths["public.ecr.aws"]. Tests run
# under `sudo -E`, so write it for the invoking user and for root.
#
# Inputs (env): AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY of a least-privilege
# identity allowed only ecr-public:GetAuthorizationToken + sts:GetServiceBearerToken.
#
# Never fails the job: with no credentials (fork PRs) or a failed token mint it
# warns and CI continues anonymously, exactly as before.
set -u

if [ -z "${AWS_ACCESS_KEY_ID:-}" ] || [ -z "${AWS_SECRET_ACCESS_KEY:-}" ]; then
    echo "ECR Public credentials not available (fork PR?); pulling anonymously."
    exit 0
fi

# ECR Public's auth API only exists in us-east-1.
token=$(AWS_DEFAULT_REGION=us-east-1 aws ecr-public get-login-password --region us-east-1 2>/dev/null) || token=""
if [ -z "$token" ]; then
    echo "::warning::could not mint an ECR Public token; pulling anonymously."
    exit 0
fi
echo "::add-mask::$token"
auth=$(printf 'AWS:%s' "$token" | base64 -w0)
echo "::add-mask::$auth"

cfg=$(printf '{"auths":{"public.ecr.aws":{"auth":"%s"}}}' "$auth")
for home in "$HOME" /root; do
    sudo install -d -m 0700 "$home/.docker"
    printf '%s' "$cfg" | sudo tee "$home/.docker/config.json" >/dev/null
    sudo chmod 0600 "$home/.docker/config.json"
done
echo "ECR Public pulls authenticated for $(id -un) and root."
