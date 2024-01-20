FROM ubuntu:latest
ARG package=release

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates

ADD target/${package}/stats-uploader /usr/bin/stats-uploader
ENTRYPOINT ["/usr/bin/stats-uploader"]