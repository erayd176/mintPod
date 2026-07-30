FROM golang:1.24-alpine AS build

WORKDIR /src
COPY go.mod main.go ./
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /mintpod-runtime .

FROM ollama/ollama:0.32.3

COPY --from=build /mintpod-runtime /usr/local/bin/mintpod-runtime
EXPOSE 8000
ENTRYPOINT ["/usr/local/bin/mintpod-runtime"]
