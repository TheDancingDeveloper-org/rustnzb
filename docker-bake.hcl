# Build the runtime image from source without access to the private CI
# registry. The cross toolchain comes from the `cross` stage of Dockerfile.ci
# instead of the pinned image in ci/images.lock. Release builds do not use this
# file.
#
#   docker buildx bake --file docker-bake.hcl --load image
target "cross" {
  dockerfile = "Dockerfile.ci"
  target     = "cross"
  platforms  = ["linux/amd64"]
}

target "image" {
  dockerfile = "Dockerfile"
  target     = "runtime"
  contexts   = { cross-image = "target:cross" }
  args       = { CROSS_IMAGE = "cross-image" }
  tags       = ["rustnzb:local"]
}
