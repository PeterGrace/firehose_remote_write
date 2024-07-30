FROM ubuntu:24.04
ARG TARGETARCH

RUN apt-get -y update && apt-get -y install ca-certificates
RUN mkdir -p /opt/app
WORKDIR /opt/app
COPY ./tools/target_arch.sh /opt/app/
COPY ./tools/entrypoint.sh /entrypoint.sh
RUN --mount=type=bind,target=/context \
 cp /context/target/$(/opt/app/target_arch.sh)/release/firehose_remote_write /opt/app/
RUN chmod a+rw -R /opt/app/*
CMD ["/entrypoint.sh"]
EXPOSE 3000
