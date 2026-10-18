set -e
source ../secrets.env

nix build .#packages.x86_64-linux.docker
docker load < result
docker push registry.murraygrov.es/discord-shard:latest

sentry-cli --auth-token ${SENTRY_TOKEN} upload-dif --org r-slash --project shard ../target/release/

ssh -4 mediaserver@home.murraygrov.es "kubectl -n r-slash rollout restart statefulset/discord-shards; kubectl -n booty-bot rollout restart statefulset/discord-shards"
