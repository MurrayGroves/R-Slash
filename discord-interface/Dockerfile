FROM ubuntu:latest

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates

ADD discord-interface /usr/bin/discord-interface
ENTRYPOINT ["/usr/bin/discord-interface"]