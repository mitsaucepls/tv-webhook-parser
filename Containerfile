FROM  debian:bookworm-slim
WORKDIR /app

COPY ./target/release/tv-webhook-parser ./tv-webhook-parser

EXPOSE 3000
CMD [ "./tv-webhook-parser" ]
