.PHONY: dev build

dev:
	cd flutter && ./run.sh

build:
	uv run python build.py --flutter --hwcodec
