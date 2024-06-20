PACKAGE="$1"
NAME="ghcr.io/dgolubets/$PACKAGE"
VERSION=$(cargo pkgid -p $PACKAGE | cut -d '#' -f 2)
IMAGE="$NAME:$VERSION"

docker build -t $IMAGE --build-arg PACKAGE=$PACKAGE .
docker push $IMAGE