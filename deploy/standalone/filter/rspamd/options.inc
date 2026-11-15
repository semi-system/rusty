# DNS (blocklists) through our own resolver only.
dns {
  nameserver = ["10.78.0.3:53"];
  timeout = 3s;
  retransmits = 2;
}
