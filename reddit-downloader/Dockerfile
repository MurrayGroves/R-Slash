FROM ubuntu:latest
ARG package=release

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates

ADD target/${package}/reddit-downloader /usr/bin/reddit-downloader
ENTRYPOINT ["/usr/bin/reddit-downloader"]