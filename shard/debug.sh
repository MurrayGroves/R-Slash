set -e
source ../secrets.env

nix build .#packages.x86_64-linux.docker-dev
docker load < result
docker push registry.murraygrov.es/discord-shard:dev

sentry-cli --auth-token ${SENTRY_TOKEN} upload-dif --org r-slash --project shard ../target/debug/

ssh -4 mediaserver@home.murraygrov.es "kubectl -n testing rollout restart statefulset/discord-shards"
