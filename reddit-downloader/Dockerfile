FROM ubuntu:latest

RUN apt update -y && apt install -y libssl-dev openssl ca-certificates ffmpeg wget gifsicle

ADD reddit-downloader /usr/bin/reddit-downloader
ENTRYPOINT ["/usr/bin/reddit-downloader"]