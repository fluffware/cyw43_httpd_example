# Simple HTTP server for RPi Pico W

The network name and password is set through environment variables at build time.
> SSID=*WiFi-network* PASS=*WiFi password* cargo build 

This includes the CYW43 firmware so it's fairly big (use --release to reduce it a bit).
When started it connects to the network and gets an addres through DHCP.
The server presents a page that lets you turn the LED on/off.
