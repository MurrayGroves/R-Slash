set -e
source ../secrets.env

nix build .#packages.x86_64-linux.docker
docker load < result
docker push registry.murraygrov.es/post-subscriber

sentry-cli --auth-token ${SENTRY_TOKEN} upload-dif --org r-slash --project post-subscriber ../target/release/

ssh -4 mediaserver@home.murraygrov.es "kubectl -n discord-bot-shared rollout restart deployment/post-subscriber"
sleep 1
ssh -4 mediaserver@home.murraygrov.es "kubectl rollout -n discord-bot-shared status deployment post-subscriber"
ssh -4 mediaserver@home.murraygrov.es "kubectl logs -n discord-bot-shared  --tail=20 --follow deployment/post-subscriber"