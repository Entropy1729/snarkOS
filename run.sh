#!/bin/bash
> pids.txt

if [ -z "$1" ]
then
	CLIENTS=3
else
	CLIENTS=$1
fi

if [ -z "$1" ]
then
	BEACON_ADDRESS="127.0.0.1:4130"
else
	BEACON_ADDRESS=$2
fi

echo "Running $CLIENTS clients for $BEACON_ADDRESS"

cargo build --release
for i in $(seq 1 $CLIENTS)
do
	./target/release/snarkos --connect_to_beacon ${BEACON_ADDRESS} --dev $((i)) --rest_port $((i + 11000)) --node 0.0.0.0:$((i+10000)) & echo "$!" >> pids.txt
done
