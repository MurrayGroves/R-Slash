FROM ubuntu:latest

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates

ADD ./post-subscriber /usr/bin/post-subscriber
ENTRYPOINT ["/usr/bin/post-subscriber"]