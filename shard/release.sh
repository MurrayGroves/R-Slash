set -e
source ../secrets.env

cargo build --release
mkdir -p build
cp ../target/release/discord-shard build/
docker build -f Dockerfile -t registry.murraygrov.es/discord-shard --network=host build/
docker push registry.murraygrov.es/discord-shard

sentry-cli --auth-token ${SENTRY_TOKEN} upload-dif --org r-slash --project shard ../target/release/

ssh -4 mediaserver@home.murraygrov.es "kubectl -n r-slash rollout restart statefulset/discord-shards; kubectl -n booty-bot rollout restart statefulset/discord-shards"