// SPDX-License-Identifier: GPL-2.0
/*
 * triskelion kernel module — main entry point.
 *
 * Registers /dev/triskelion char device. Each open() creates an isolated
 * server context (handle table, message queues, thread state). Wine
 * processes issue ioctls instead of wineserver socket calls.
 */

#include <linux/module.h>
#include <linux/miscdevice.h>
#include <linux/fs.h>
#include <linux/slab.h>

#include "triskelion.h"
#include "triskelion_internal.h"

MODULE_LICENSE("GPL");
MODULE_AUTHOR("amphetamine");
MODULE_DESCRIPTION("triskelion — wineserver in kernel space");
MODULE_VERSION("0.1.0");

static int max_handles = 4096;
module_param(max_handles, int, 0644);
MODULE_PARM_DESC(max_handles, "Maximum handles per server context (default: 4096)");

static bool debug;
module_param(debug, bool, 0644);
MODULE_PARM_DESC(debug, "Enable verbose debug logging");

static int triskelion_open(struct inode *inode, struct file *file)
{
	struct triskelion_ctx *ctx;
	int ret;

	ctx = kzalloc(sizeof(*ctx), GFP_KERNEL);
	if (!ctx)
		return -ENOMEM;

	spin_lock_init(&ctx->lock);

	ret = triskelion_handles_init(&ctx->handles, max_handles);
	if (ret) {
		kfree(ctx);
		return ret;
	}

	triskelion_queues_init(&ctx->queues);

	file->private_data = ctx;

	if (debug)
		pr_info("triskelion: context created (pid %d)\n", current->pid);

	return 0;
}

static int triskelion_release(struct inode *inode, struct file *file)
{
	struct triskelion_ctx *ctx = file->private_data;

	if (!ctx)
		return 0;

	triskelion_queues_destroy(&ctx->queues);
	triskelion_handles_destroy(&ctx->handles);
	kfree(ctx);

	if (debug)
		pr_info("triskelion: context destroyed (pid %d)\n", current->pid);

	return 0;
}

static long triskelion_ioctl(struct file *file, unsigned int cmd,
			     unsigned long arg)
{
	struct triskelion_ctx *ctx = file->private_data;

	if (!ctx)
		return -EINVAL;

	return triskelion_dispatch(ctx, cmd, arg);
}

static const struct file_operations triskelion_fops = {
	.owner          = THIS_MODULE,
	.open           = triskelion_open,
	.release        = triskelion_release,
	.unlocked_ioctl = triskelion_ioctl,
	.compat_ioctl   = triskelion_ioctl,
};

static struct miscdevice triskelion_misc = {
	.minor = MISC_DYNAMIC_MINOR,
	.name  = TRISKELION_DEVICE_NAME,
	.fops  = &triskelion_fops,
	.mode  = 0666,
};

static int __init triskelion_init(void)
{
	int ret;

	ret = triskelion_sync_init();
	if (ret) {
		pr_err("triskelion: failed to create slab caches\n");
		return ret;
	}

	ret = misc_register(&triskelion_misc);
	if (ret) {
		pr_err("triskelion: failed to register /dev/%s\n",
		       TRISKELION_DEVICE_NAME);
		triskelion_sync_exit();
		return ret;
	}

	pr_info("triskelion: loaded (/dev/%s, max_handles=%d)\n",
		TRISKELION_DEVICE_NAME, max_handles);
	return 0;
}

static void __exit triskelion_exit(void)
{
	misc_deregister(&triskelion_misc);
	triskelion_sync_exit();
	pr_info("triskelion: unloaded\n");
}

module_init(triskelion_init);
module_exit(triskelion_exit);
