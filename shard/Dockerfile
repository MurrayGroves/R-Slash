FROM ubuntu:latest
ARG package=release

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
ADD "http://security.ubuntu.com/ubuntu/pool/main/c/ca-certificates/ca-certificates-udeb_20211016ubuntu0.18.04.1_all.udeb" .
RUN dpkg -i ca-certificates*deb

ADD target/${package}/discord-shard /usr/bin/discord-shard
ENTRYPOINT ["/usr/bin/discord-shard"]