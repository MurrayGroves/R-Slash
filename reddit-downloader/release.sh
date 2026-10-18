#!/bin/sh
set -e
source ../secrets.env

nix build .#packages.x86_64-linux.docker
docker load < result
docker push registry.murraygrov.es/reddit-downloader

docker build -f Dockerfile -t registry.murraygrov.es/discord-shard:dev --network=host build/
docker push registry.murraygrov.es/discord-shard:dev
ssh -4 mediaserver@home.murraygrov.es "kubectl -n discord-bot-shared rollout restart deployment/reddit-downloader"
sleep 1
ssh -4 mediaserver@home.murraygrov.es "kubectl rollout -n discord-bot-shared status deployment reddit-downloader"
ssh -4 mediaserver@home.murraygrov.es "kubectl logs -n discord-bot-shared  --tail=20 --follow deployment/reddit-downloader"
