#!/bin/bash
> pids.txt

cargo build --release
for i in {1..2}
do
	./target/release/snarkos --connect_to_beacon 127.0.0.1:4130 --dev $((i)) --rest_port $((i + 11000)) --node 0.0.0.0:$((i+10000)) & echo "$!" >> pids.txt
done
