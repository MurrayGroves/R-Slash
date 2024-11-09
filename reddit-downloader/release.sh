set -e
source ./secrets.env

RUSTFLAGS="--cfg tokio_unstable" cargo build --release
docker build -f Dockerfile -t registry.murraygrov.es/reddit-downloader ../target/release
docker push registry.murraygrov.es/reddit-downloader

sentry-cli --auth-token ${SENTRY_TOKEN} upload-dif --org r-slash --project downloader ../target/release/

ssh -4 mediaserver@home.murraygrov.es "kubectl -n discord-bot-shared rollout restart deployment/reddit-downloader"
sleep 1
ssh -4 mediaserver@home.murraygrov.es "kubectl rollout -n discord-bot-shared status deployment reddit-downloader"
ssh -4 mediaserver@home.murraygrov.es "kubectl logs -n discord-bot-shared  --tail=20 --follow deployment/reddit-downloader"
