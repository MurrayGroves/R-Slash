#!/bin/sh
set -e

cargo build --release
docker build -t registry.murraygrov.es/auto-poster -f Dockerfile target/release
docker push registry.murraygrov.es/auto-poster
ssh -4 mediaserver@home.murraygrov.es "kubectl -n discord-bot-shared rollout restart deployment/auto-poster"
