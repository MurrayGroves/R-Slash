FROM ubuntu:latest

ADD base.html base.html

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates

ADD ./target/release/mp4-opengraph /usr/bin/mp4-opengraph
ENTRYPOINT ["/usr/bin/mp4-opengraph"]