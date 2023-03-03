FROM ubuntu:latest

#ADD "http://security.debian.org/debian-security/pool/updates/main/o/openssl/libssl1.1_1.1.0l-1~deb9u6_amd64.deb" .
#RUN dpkg -i libssl1.1*deb
ADD "http://security.ubuntu.com/ubuntu/pool/main/o/openssl/libssl1.1_1.1.1f-1ubuntu2.16_amd64.deb" .
RUN dpkg -i libssl*deb
ADD "http://security.ubuntu.com/ubuntu/pool/main/o/openssl/openssl_1.1.1f-1ubuntu2.16_amd64.deb" .
RUN dpkg -i openssl*deb
ADD "http://security.ubuntu.com/ubuntu/pool/main/c/ca-certificates/ca-certificates-udeb_20211016ubuntu0.18.04.1_all.udeb" .
RUN dpkg -i ca-certificates*deb

ADD target/release/role-detector /usr/bin/role-detector
ENTRYPOINT ["/usr/bin/role-detector"]