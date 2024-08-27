FROM ubuntu:latest

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates

ADD ./auto_poster /usr/bin/auto-poster
ENTRYPOINT ["/usr/bin/auto-poster"]
