dev-no-redoxer = нужен redoxer: cargo install redoxer --locked && redoxer toolchain
dev-no-reader = сканер { $device } не подключён
dev-no-access = нет доступа на запись к { $node }: поставь scripts/70-redox-passthrough.rules на время сеанса и потом удали — оно даёт любой программе в твоём сеансе прямой доступ к сканеру
dev-no-docker = нужен docker
