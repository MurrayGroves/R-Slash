FROM ubuntu:latest

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates
RUN apt install iproute2 -y
RUN apt install -y iputils-ping
RUN apt install -y inetutils-traceroute
ADD ./reddit-proxy /usr/bin/reddit-proxy
ENTRYPOINT ["/usr/bin/reddit-proxy"]
