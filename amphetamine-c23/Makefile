KDIR ?= /lib/modules/$(shell uname -r)/build

# Match compiler toolchain to kernel build (clang+lld for CachyOS/LLVM kernels)
CC ?= clang
LD ?= ld.lld

all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) CC=$(CC) LD=$(LD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) clean

install: all
	sudo cp triskelion_kmod.ko /lib/modules/$(shell uname -r)/extra/
	sudo depmod -a
	sudo modprobe triskelion_kmod

uninstall:
	sudo rmmod triskelion_kmod 2>/dev/null || true
	sudo rm -f /lib/modules/$(shell uname -r)/extra/triskelion_kmod.ko
	sudo depmod -a

.PHONY: all clean install uninstall
