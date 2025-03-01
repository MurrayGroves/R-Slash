set -e
source ../secrets.env

cargo build
mkdir -p build
cp ../target/debug/discord-shard build/

docker build -f Dockerfile -t registry.murraygrov.es/discord-shard:dev --network=host build/
docker push registry.murraygrov.es/discord-shard:dev

sentry-cli --auth-token ${SENTRY_TOKEN} upload-dif --org r-slash --project shard ../target/debug/

ssh -4 mediaserver@home.murraygrov.es "kubectl -n testing rollout restart statefulset/discord-shards"