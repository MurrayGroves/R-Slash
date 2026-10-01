#!/bin/bash
set -e
source ../secrets.env

nix build
docker load < result
docker push registry.murraygrov.es/auto-poster

sentry-cli --auth-token ${SENTRY_TOKEN} upload-dif --org r-slash --project auto-poster ../target/release/

ssh -4 mediaserver@home.murraygrov.es "kubectl -n discord-bot-shared rollout restart deployment/auto-poster"
