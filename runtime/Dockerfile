FROM golang:1.24-alpine@sha256:8bee1901f1e530bfb4a7850aa7a479d17ae3a18beb6e09064ed54cfd245b7191 AS build

WORKDIR /src
COPY go.mod main.go ./
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /mintpod-runtime .

FROM ollama/ollama:0.32.3@sha256:ec24bcaa2a810eb74171ce7c517813ef4821ed678988845e8d76cf62467036d4

COPY --from=build /mintpod-runtime /usr/local/bin/mintpod-runtime
EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/mintpod-runtime"]
