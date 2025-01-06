#!/bin/sh
set -e

cargo build --release --bin discord-interface
docker build -t registry.murraygrov.es/discord-interface -f Dockerfile ../target/release
docker push registry.murraygrov.es/discord-interface
ssh -4 mediaserver@home.murraygrov.es "kubectl -n r-slash rollout restart deployment/discord-interface; kubectl -n booty-bot rollout restart deployment/discord-interface"