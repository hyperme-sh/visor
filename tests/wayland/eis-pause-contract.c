/* A relative-pointer sender and a stand-in compositor on a private EIS fd. */
#include <libei.h>
#include <libeis.h>

#include <poll.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>

#define CHECK(condition)                                                        \
    do {                                                                        \
        if (!(condition)) {                                                     \
            fprintf(stderr, "line %d: %s\n", __LINE__, #condition);             \
            exit(EXIT_FAILURE);                                                 \
        }                                                                       \
    } while (0)

struct fixture {
    struct ei *sender;
    struct eis *server;
    struct eis_client *client;
    struct ei_device *pointer;
    struct eis_device *server_pointer;
    bool paused;
    bool sender_saw_pause;
    bool sender_disconnected;
    unsigned accepted_motion;
    unsigned discarded_motion;
};

static void drain_server_events(struct fixture *fixture)
{
    struct eis_event *event;
    while ((event = eis_get_event(fixture->server))) {
        switch (eis_event_get_type(event)) {
        case EIS_EVENT_CLIENT_CONNECT: {
            struct eis_client *client = eis_event_get_client(event);
            CHECK(eis_client_is_sender(client));
            fixture->client = eis_client_ref(client);
            eis_client_connect(client);
            struct eis_seat *seat = eis_client_new_seat(client, "contract seat");
            eis_seat_configure_capability(seat, EIS_DEVICE_CAP_POINTER);
            eis_seat_add(seat);
            eis_seat_unref(seat);
            break;
        }
        case EIS_EVENT_SEAT_BIND: {
            CHECK(fixture->server_pointer == NULL);
            struct eis_device *pointer = eis_seat_new_device(eis_event_get_seat(event));
            eis_device_configure_name(pointer, "contract pointer");
            eis_device_configure_capability(pointer, EIS_DEVICE_CAP_POINTER);
            eis_device_add(pointer);
            eis_device_resume(pointer);
            fixture->server_pointer = pointer;
            break;
        }
        case EIS_EVENT_POINTER_MOTION:
            if (fixture->paused) {
                fixture->discarded_motion++;
            } else {
                fixture->accepted_motion++;
            }
            break;
        default:
            break;
        }
        eis_event_unref(event);
    }
}

static void dispatch_sender(struct fixture *fixture)
{
    struct ei_event *event;
    ei_dispatch(fixture->sender);
    while ((event = ei_get_event(fixture->sender))) {
        switch (ei_event_get_type(event)) {
        case EI_EVENT_SEAT_ADDED:
            ei_seat_bind_capabilities(ei_event_get_seat(event), EI_DEVICE_CAP_POINTER, NULL);
            break;
        case EI_EVENT_DEVICE_RESUMED:
            CHECK(fixture->pointer == NULL);
            fixture->pointer = ei_device_ref(ei_event_get_device(event));
            break;
        case EI_EVENT_DEVICE_PAUSED:
            CHECK(ei_event_get_device(event) == fixture->pointer);
            fixture->sender_saw_pause = true;
            break;
        case EI_EVENT_DISCONNECT:
            fixture->sender_disconnected = true;
            break;
        default:
            break;
        }
        ei_event_unref(event);
    }
}

static void send_motion(const struct fixture *fixture)
{
    ei_device_pointer_motion(fixture->pointer, 1, 0);
    ei_device_frame(fixture->pointer, ei_now(fixture->sender));
}

static void wait_for_fd(int fd)
{
    struct pollfd ready = {.fd = fd, .events = POLLIN};
    CHECK(poll(&ready, 1, 1000) == 1 && (ready.revents & POLLIN));
}

int main(void)
{
    struct fixture fixture = {
        .sender = ei_new_sender(NULL),
        .server = eis_new(NULL),
    };
    CHECK(fixture.sender && fixture.server);
    CHECK(eis_setup_backend_fd(fixture.server) == 0);
    int fd = eis_backend_fd_add_client(fixture.server);
    CHECK(fd >= 0);
    ei_configure_name(fixture.sender, "visor pause contract");
    CHECK(ei_setup_backend_fd(fixture.sender, fd) == 0);

    struct pollfd peers[] = {
        {.fd = eis_get_fd(fixture.server), .events = POLLIN},
        {.fd = ei_get_fd(fixture.sender), .events = POLLIN},
    };
    for (unsigned tick = 0; !fixture.pointer && tick < 20; ++tick) {
        CHECK(poll(peers, 2, 1000) > 0);
        if (peers[0].revents & POLLIN) {
            eis_dispatch(fixture.server);
            drain_server_events(&fixture);
        }
        if (peers[1].revents & POLLIN) {
            dispatch_sender(&fixture);
        }
    }
    CHECK(fixture.pointer && fixture.server_pointer && fixture.client);
    CHECK(!fixture.sender_saw_pause && !fixture.sender_disconnected);
    ei_device_start_emulating(fixture.pointer, 1);
    send_motion(&fixture);
    wait_for_fd(eis_get_fd(fixture.server));
    eis_dispatch(fixture.server);
    drain_server_events(&fixture);
    CHECK(fixture.accepted_motion == 1);

    /* Queue motion in libeis before the compositor gets a chance to apply it. */
    send_motion(&fixture);
    wait_for_fd(eis_get_fd(fixture.server));
    eis_dispatch(fixture.server);

    /* A local takeover latches the gate before sending any EI notification. */
    fixture.paused = true;
    eis_device_pause(fixture.server_pointer);
    drain_server_events(&fixture);
    CHECK(fixture.accepted_motion == 1 && fixture.discarded_motion == 1);
    dispatch_sender(&fixture);

    /* Disconnect works even on libeis versions that skip an active pause. */
    eis_client_disconnect(fixture.client);
    wait_for_fd(ei_get_fd(fixture.sender));
    dispatch_sender(&fixture);
    CHECK(fixture.sender_disconnected);
    printf("Active pause event: %s\n", fixture.sender_saw_pause ? "received" : "absent");
    puts("Queued motion discarded and EI sender notified of disconnect");

    ei_device_unref(fixture.pointer);
    eis_device_unref(fixture.server_pointer);
    eis_client_unref(fixture.client);
    ei_unref(fixture.sender);
    eis_unref(fixture.server);
    return EXIT_SUCCESS;
}
