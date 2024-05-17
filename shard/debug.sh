set -e
source ./secrets.env

cargo build
docker build -f Dockerfile -t registry.murraygrov.es/discord-shard:dev target/debug
docker push registry.murraygrov.es/discord-shard:dev

sentry-cli --auth-token ${SENTRY_TOKEN} upload-dif --org r-slash --project shard target/debug/

ssh -4 mediaserver@home.murraygrov.es "kubectl -n testing rollout restart statefulset/discord-shards"