#!/bin/sh

cargo build --release
docker build -t registry.murraygrov.es/post-subscriber -f Dockerfile target/release
docker push registry.murraygrov.es/post-subscriber
ssh -4 mediaserver@home.murraygrov.es "kubectl -n discord-bot-shared rollout restart deployment/post-subscriber"