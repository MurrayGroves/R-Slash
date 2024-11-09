#!/bin/bash
set -e
source ../secrets.env

cargo build --release
mkdir -p build
cp ../target/release/auto-poster build/
docker build -t registry.murraygrov.es/auto-poster -f Dockerfile build/
docker push registry.murraygrov.es/auto-poster

sentry-cli --auth-token ${SENTRY_TOKEN} upload-dif --org r-slash --project auto-poster ../target/release/

ssh -4 mediaserver@home.murraygrov.es "kubectl -n discord-bot-shared rollout restart deployment/auto-poster"
