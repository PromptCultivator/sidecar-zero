# Spike S4: Go reader (github.com/apache/arrow-go/v18).
FROM golang:1.27
WORKDIR /s4/go
COPY go.mod go.sum ./
RUN --mount=type=cache,target=/go/pkg/mod,id=scz-spike-s4-gomod go mod download
COPY main.go ./
# -p 4: other spikes build on the same VM.
RUN --mount=type=cache,target=/go/pkg/mod,id=scz-spike-s4-gomod \
    --mount=type=cache,target=/root/.cache/go-build,id=scz-spike-s4-gobuild \
    go build -p 4 -o /usr/local/bin/s4-go . \
    && (go version; go list -m github.com/apache/arrow-go/v18) > /s4/go/versions.txt
ENTRYPOINT ["/usr/local/bin/s4-go"]
