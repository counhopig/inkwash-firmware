# Device flashing safety

- The authorized firmware target is the Zectrix Note 4 connected through USB-A with MAC address `20:6E:F1:B4:7D:E4`.
- Another ESP32 device may be connected at the same time. Never select a flashing target solely from a serial-port name such as `/dev/ttyACM0`.
- Before every flash, verify that the connected device is the authorized Zectrix Note 4. If its identity cannot be confirmed, do not flash.
- Flash this board only as ESP32-S3 with 16 MB flash, DIO mode, 80 MHz flash frequency, and `rust-firmware/partitions.csv`.
- Never flash Note 4 firmware to a Note 4C or another ESP32 board.
