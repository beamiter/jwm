#!/usr/bin/env python3
"""A private-session MPRIS player for real sidebar media-command acceptance.

The service emits no audio. Its persisted method log proves actual D-Bus
transport and state propagation, separately from the physical ALSA tests.
"""
import json
import os
from pathlib import Path
import signal
import sys
import dbus
import dbus.mainloop.glib
import dbus.service
from gi.repository import GLib

PLAYER = 'org.mpris.MediaPlayer2.Player'
ROOT = 'org.mpris.MediaPlayer2'
PROPERTIES = 'org.freedesktop.DBus.Properties'


def main():
    destination = Path(sys.argv[1])
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    loop = GLib.MainLoop()
    bus = dbus.SessionBus()
    bus.call_on_disconnection(lambda _: loop.quit())
    name = dbus.service.BusName('org.mpris.MediaPlayer2.jwm_shell_fixture', bus=bus)
    state = {'ready': True, 'pid': os.getpid(), 'events': [], 'track': 0, 'playback': 'Playing'}

    def persist():
        temporary = destination.with_suffix('.tmp')
        temporary.write_text(json.dumps(state) + '\n')
        temporary.replace(destination)

    class Player(dbus.service.Object):
        def properties(self, interface):
            if interface == ROOT:
                return {'Identity': 'JWM shared shell fixture', 'CanQuit': True, 'CanRaise': False,
                        'HasTrackList': False, 'SupportedUriSchemes': dbus.Array([], signature='s'),
                        'SupportedMimeTypes': dbus.Array([], signature='s')}
            if interface == PLAYER:
                return {'PlaybackStatus': state['playback'], 'CanControl': True, 'CanPlay': True,
                        'CanPause': True, 'CanGoNext': True, 'CanGoPrevious': True,
                        'Metadata': dbus.Dictionary({'mpris:trackid': dbus.ObjectPath('/jwm/track/' + str(state['track'])),
                            'xesam:title': 'Shared sidebar media track ' + str(state['track']),
                            'xesam:artist': dbus.Array(['JWM fixture'], signature='s')}, signature='sv'),
                        'Volume': dbus.Double(0.5), 'Position': dbus.Int64(0), 'Rate': dbus.Double(1.0)}
            return {}

        @dbus.service.method(PROPERTIES, in_signature='s', out_signature='a{sv}')
        def GetAll(self, interface):
            return self.properties(interface)

        @dbus.service.method(PROPERTIES, in_signature='ss', out_signature='v')
        def Get(self, interface, property_name):
            return self.properties(interface)[property_name]

        @dbus.service.method(PROPERTIES, in_signature='ssv', out_signature='')
        def Set(self, interface, property_name, value):
            pass

        @dbus.service.signal(PROPERTIES, signature='sa{sv}as')
        def PropertiesChanged(self, interface, changed, invalidated):
            pass

        def update(self, command):
            state['events'].append(command)
            if command == 'Next':
                state['track'] += 1
            elif command == 'Previous':
                state['track'] = max(0, state['track'] - 1)
            elif command == 'PlayPause':
                state['playback'] = 'Paused' if state['playback'] == 'Playing' else 'Playing'
            else:
                state['playback'] = 'Playing' if command == 'Play' else 'Paused'
            persist()
            self.PropertiesChanged(PLAYER, self.properties(PLAYER), dbus.Array([], signature='s'))

        @dbus.service.method(PLAYER)
        def Next(self): self.update('Next')
        @dbus.service.method(PLAYER)
        def Previous(self): self.update('Previous')
        @dbus.service.method(PLAYER)
        def PlayPause(self): self.update('PlayPause')
        @dbus.service.method(PLAYER)
        def Play(self): self.update('Play')
        @dbus.service.method(PLAYER)
        def Pause(self): self.update('Pause')
        @dbus.service.method(ROOT)
        def Raise(self): pass
        @dbus.service.method(ROOT)
        def Quit(self): loop.quit()

    player = Player(name, '/org/mpris/MediaPlayer2')
    persist()
    signal.signal(signal.SIGTERM, lambda *_: loop.quit())
    loop.run()


if __name__ == '__main__':
    main()
