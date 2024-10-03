set -e

cargo build --release
docker build -f Dockerfile -t registry.murraygrov.es/mp4-opengraph .
docker push registry.murraygrov.es/mp4-opengraph


ssh -4 mediaserver@home.murraygrov.es "kubectl -n discord-bot-shared rollout restart deployment/mp4-opengraph"