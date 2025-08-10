set -e
source ../secrets.env

cargo build --release
docker build -t registry.murraygrov.es/reddit-proxy -f Dockerfile --network=host ../target/release
docker push registry.murraygrov.es/reddit-proxy

sentry-cli --auth-token ${SENTRY_TOKEN} upload-dif --org r-slash --project reddit-proxy ../target/release/

ssh -4 mediaserver@home.murraygrov.es "kubectl -n discord-bot-shared rollout restart deployment/reddit-proxy"
sleep 1
ssh -4 mediaserver@home.murraygrov.es "kubectl rollout -n discord-bot-shared status deployment reddit-proxy"
ssh -4 mediaserver@home.murraygrov.es "kubectl logs -n discord-bot-shared  --tail=20 --follow deployment/reddit-proxy"