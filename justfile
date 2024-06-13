commit := `git rev-parse HEAD`
shortcommit := `git rev-parse HEAD`
transport := "docker://"
registry := "docker.io"
image := "petergrace/firehose_to_prom_push"
tag := `git describe --tags 2>/dev/null|| echo dev`


default: build image

build:
  cross build --release --target x86_64-unknown-linux-gnu

image:
  docker buildx build --push --platform linux/amd64 \
  -t {{registry}}/{{image}}:latest \
  -t {{registry}}/{{image}}:{{shortcommit}} \
  -t {{registry}}/{{image}}:{{commit}} \
  -t {{registry}}/{{image}}:{{tag}} \
  .
  
release-patch:
  cargo release --no-publish --no-verify patch --execute
release-minor:
  cargo release --no-publish --no-verify minor --execute
release-major:
  cargo release --no-publish --no-verify major --execute

