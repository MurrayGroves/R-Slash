FROM ubuntu:latest

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates

ADD target/release/discord-shard /usr/bin/discord-shard
ENTRYPOINT ["/usr/bin/discord-shard"]