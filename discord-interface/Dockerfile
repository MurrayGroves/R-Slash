FROM ubuntu:latest


RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
ADD "http://security.ubuntu.com/ubuntu/pool/main/c/ca-certificates/ca-certificates-udeb_20211016ubuntu0.18.04.1_all.udeb" .
RUN dpkg -i ca-certificates*deb

ADD target/release/discord-interface /usr/bin/discord-interface
ENTRYPOINT ["/usr/bin/discord-interface"]