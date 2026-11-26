/* Test client: transfer a real Unix FD and await a directed portal reply. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <gio/gio.h>
#include <gio/gunixfdlist.h>
#include <sys/stat.h>
#include <unistd.h>

#define PORTAL "org.freedesktop.portal.Desktop"
#define ROOT "/org/freedesktop/portal/desktop"
#define REQUEST "org.freedesktop.portal.Request"

struct response {
    GMainLoop *loop;
    guint code;
    gboolean arrived;
};

static void
on_response(GDBusConnection *bus G_GNUC_UNUSED,
            const gchar *sender G_GNUC_UNUSED,
            const gchar *path G_GNUC_UNUSED,
            const gchar *interface G_GNUC_UNUSED,
            const gchar *signal G_GNUC_UNUSED,
            GVariant *args, gpointer data)
{
    struct response *response = data;
    g_autoptr(GVariant) results = NULL;
    g_variant_get(args, "(u@a{sv})", &response->code, &results);
    response->arrived = TRUE;
    g_main_loop_quit(response->loop);
}

static gboolean
on_timeout(gpointer loop)
{
    g_main_loop_quit(loop);
    return G_SOURCE_CONTINUE; /* Removed by the main function after returning. */
}

int
main(int argc, char **argv)
{
    if (argc != 2) {
        g_printerr("usage: portal-open-directory PATH\n");
        return 2;
    }
    g_autoptr(GError) error = NULL;
    g_autoptr(GDBusConnection) bus = g_bus_get_sync(G_BUS_TYPE_SESSION, NULL, &error);
    if (!bus) {
        g_printerr("cannot open private session bus: %s\n", error->message);
        return 1;
    }
    const gchar *unique = g_dbus_connection_get_unique_name(bus);
    if (!unique || unique[0] != ':')
        return 1;
    g_autofree gchar *sender = g_strdup(unique + 1);
    g_strdelimit(sender, ".", '_');
    const gchar token[] = "jwm_files_fd_smoke";
    g_autofree gchar *expected = g_strdup_printf(ROOT "/request/%s/%s", sender, token);

    g_autoptr(GUnixFDList) fds = g_unix_fd_list_new();
    /* Use a readable descriptor, as browser callers do. Portal 1.18
     * explicitly rejects the O_PATH | O_NOFOLLOW flag combination. */
    int fd = open(argv[1], O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        g_printerr("cannot hold fixture descriptor: %s\n", g_strerror(errno));
        return 1;
    }
    struct stat st;
    if (fstat(fd, &st) != 0 || !S_ISREG(st.st_mode)) {
        close(fd);
        g_printerr("OpenDirectory smoke expects a regular fixture file\n");
        return 1;
    }
    int index = g_unix_fd_list_append(fds, fd, &error);
    close(fd);
    if (index < 0) {
        g_printerr("cannot append fixture descriptor: %s\n", error->message);
        return 1;
    }

    g_autoptr(GMainLoop) loop = g_main_loop_new(NULL, FALSE);
    struct response response = { .loop = loop };
    guint listener = g_dbus_connection_signal_subscribe(
        bus, PORTAL, REQUEST, "Response", expected, NULL,
        G_DBUS_SIGNAL_FLAGS_NONE, on_response, &response, NULL);
    GVariantBuilder options;
    g_variant_builder_init(&options, G_VARIANT_TYPE_VARDICT);
    g_variant_builder_add(&options, "{sv}", "handle_token", g_variant_new_string(token));
    g_autoptr(GVariant) opened = g_dbus_connection_call_with_unix_fd_list_sync(
        bus, PORTAL, ROOT, "org.freedesktop.portal.OpenURI", "OpenDirectory",
        g_variant_new("(sh@a{sv})", "", index, g_variant_builder_end(&options)),
        G_VARIANT_TYPE("(o)"), G_DBUS_CALL_FLAGS_NO_AUTO_START, 5000,
        fds, NULL, NULL, &error);
    if (!opened) {
        g_dbus_connection_signal_unsubscribe(bus, listener);
        g_printerr("OpenDirectory D-Bus call failed: %s\n", error->message);
        return 1;
    }
    const gchar *handle;
    g_variant_get(opened, "(&o)", &handle);
    if (g_strcmp0(expected, handle) != 0) {
        g_dbus_connection_signal_unsubscribe(bus, listener);
        g_printerr("OpenDirectory returned a different request handle\n");
        return 1;
    }
    guint deadline = g_timeout_add_seconds(8, on_timeout, loop);
    g_main_loop_run(loop);
    g_source_remove(deadline);
    g_dbus_connection_signal_unsubscribe(bus, listener);
    if (!response.arrived || response.code != 0) {
        g_printerr("OpenDirectory did not succeed (response received=%d, code=%u)\n",
                   response.arrived, response.code);
        return 1;
    }
    return 0;
}
