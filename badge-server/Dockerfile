FROM ubuntu:latest
ARG package=release

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates

ADD target/${package}/badge-server /usr/bin/badge-server
ENTRYPOINT ["/usr/bin/badge-server"]