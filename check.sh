leader=107.21.85.246
export ips="54.209.198.32 35.174.139.220 54.89.72.66 52.201.234.227 54.234.242.128 52.23.253.49 52.90.134.252 18.206.88.129 34.238.38.197"

for ff in $ips; do
    ssh $ff -n "cat ~/solana/validator.json"
    ssh $ff -n "ps aux | grep solana"
done
