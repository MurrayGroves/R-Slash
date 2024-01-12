FROM ubuntu:latest
ARG package=release

RUN apt update -y
RUN apt install -y libssl-dev
RUN apt install -y openssl
RUN apt install -y ca-certificates
RUN apt install -y ffmpeg
RUN apt install -y wget
RUN apt install -y mediainfo

ADD target/${package}/reddit-downloader /usr/bin/reddit-downloader
ENTRYPOINT ["/usr/bin/reddit-downloader"]