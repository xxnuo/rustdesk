.PHONY: dev build

dev:
	cd flutter && ./run.sh

build:
	python3 build.py --flutter --hwcodec
