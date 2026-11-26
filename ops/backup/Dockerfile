# Postgres 18 client tools must be the server's major version or newer; Railway runs postgres-ssl:18.
FROM postgres:18-alpine
RUN apk add --no-cache bash age aws-cli coreutils curl
COPY backup.sh prune.sh /usr/local/bin/
USER postgres
CMD ["backup.sh"]
