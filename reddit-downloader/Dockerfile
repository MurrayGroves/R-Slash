FROM ubuntu:latest
ARG package=release

#ADD "http://security.ubuntu.com/ubuntu/pool/main/o/openssl/libssl1.1_1.1.1-1ubuntu2.1~18.04.21_amd64.deb" .
#RUN dpkg -i libssl*deb
#ADD "http://security.ubuntu.com/ubuntu/pool/main/o/openssl/openssl_1.1.1-1ubuntu2.1~18.04.21_amd64.deb" .
#RUN dpkg -i openssl*deb
RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates

ADD target/${package}/reddit-downloader /usr/bin/reddit-downloader
ENTRYPOINT ["/usr/bin/reddit-downloader"]