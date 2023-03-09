FROM ubuntu:latest


ADD "http://security.debian.org/debian-security/pool/updates/main/o/openssl/libssl1.1_1.1.0l-1~deb9u6_amd64.deb" .
RUN dpkg -i libssl1.1*deb

RUN apt-get -qq update
RUN apt-get -qq install -y curl
RUN apt-get -qq install -y ca-certificates
RUN curl -fsSLo /usr/share/keyrings/kubernetes-archive-keyring.gpg https://packages.cloud.google.com/apt/doc/apt-key.gpg
RUN echo "deb [signed-by=/usr/share/keyrings/kubernetes-archive-keyring.gpg] https://apt.kubernetes.io/ kubernetes-xenial main" | tee /etc/apt/sources.list.d/kubernetes.list
RUN apt-get -qq update
RUN apt-get -qq install -y kubectl

ADD target/release/discord-interface /usr/bin/discord-interface
ENTRYPOINT ["/usr/bin/discord-interface"]