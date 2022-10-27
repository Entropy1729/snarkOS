CLIENTS?=10
BEACON_ADDRESS=http://127.0.0.1:4130

build:
	cargo build --release

run_multiple:
	./run.sh ${CLIENTS} ${BEACON_ADDRESS}

clean:
	./clean.sh
