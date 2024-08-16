// Package zipstream
// A streaming zip reader that specifically supports GitHub Actions artifacts files.
// The standard library's `archive/zip` uses the central directory at the end of the file,
// so it would require downloading the entire file before reading it.
// This package reads the local file headers and supports DEFLATE streams with an unknown size,
// allowing us to read files as the response is being downloaded.
package zipstream

import (
	"archive/zip"
	"bufio"
	"compress/flate"
	"encoding/binary"
	"fmt"
	"io"
)

const (
	headerIdentifierLen      = 4
	fileHeaderLen            = 26
	dataDescriptorLen        = 16 // four uint32: descriptor signature, crc32, compressed size, size
	fileHeaderSignature      = 0x04034b50
	directoryHeaderSignature = 0x02014b50
	directoryEndSignature    = 0x06054b50
	dataDescriptorSignature  = 0x08074b50
	zip64ExtraID             = 0x0001 // Zip64 extended information
)

type Reader struct {
	r            io.Reader
	rBuf         *bufio.Reader
	fr           io.ReadCloser
	localFileEnd bool
	curEntry     *Entry
}

func NewReader(r io.Reader) *Reader {
	return &Reader{
		r:  r,
		fr: nil,
	}
}

type Entry struct {
	zip.FileHeader
	r io.Reader
}

func (e *Entry) hasDataDescriptor() bool {
	return e.Flags&8 != 0
}

// IsDir just simply check whether the entry name ends with "/"
func (e *Entry) IsDir() bool {
	return len(e.Name) > 0 && e.Name[len(e.Name)-1] == '/'
}

func (e *Entry) Read(p []byte) (n int, err error) {
	return e.r.Read(p)
}

//goland:noinspection GoDeprecation
func (z *Reader) readEntry() (*Entry, error) {
	buf := make([]byte, fileHeaderLen)
	if _, err := io.ReadFull(z.r, buf); err != nil {
		return nil, fmt.Errorf("unable to read local file header: %w", err)
	}

	lr := readBuf(buf)
	readerVersion := lr.uint16()
	flags := lr.uint16()
	method := lr.uint16()
	modifiedTime := lr.uint16()
	modifiedDate := lr.uint16()
	crc32Sum := lr.uint32()
	compressedSize := lr.uint32()
	uncompressedSize := lr.uint32()
	filenameLen := int(lr.uint16())
	extraAreaLen := int(lr.uint16())

	entry := &Entry{
		FileHeader: zip.FileHeader{
			ReaderVersion:      readerVersion,
			Flags:              flags,
			Method:             method,
			ModifiedTime:       modifiedTime,
			ModifiedDate:       modifiedDate,
			CRC32:              crc32Sum,
			CompressedSize:     compressedSize,
			UncompressedSize:   uncompressedSize,
			CompressedSize64:   uint64(compressedSize),
			UncompressedSize64: uint64(uncompressedSize),
		},
		r: nil,
	}

	nameAndExtraBuf := make([]byte, filenameLen+extraAreaLen)
	if _, err := io.ReadFull(z.r, nameAndExtraBuf); err != nil {
		return nil, fmt.Errorf("unable to read entry name and extra area: %w", err)
	}

	entry.Name = string(nameAndExtraBuf[:filenameLen])
	entry.Extra = nameAndExtraBuf[filenameLen:]

	entry.NonUTF8 = flags&0x800 == 0
	if flags&1 == 1 {
		return nil, fmt.Errorf("encrypted ZIP entry not supported")
	}
	if flags&8 == 8 && method != zip.Deflate {
		return nil, fmt.Errorf("only DEFLATED entries can have data descriptor")
	}

	needCSize := entry.CompressedSize == ^uint32(0)
	needUSize := entry.UncompressedSize == ^uint32(0)

	ler := readBuf(entry.Extra)
	for len(ler) >= 4 { // need at least tag and size
		fieldTag := ler.uint16()
		fieldSize := int(ler.uint16())
		if len(ler) < fieldSize {
			break
		}
		fieldBuf := ler.sub(fieldSize)

		switch fieldTag {
		case zip64ExtraID:
			// update directory values from the zip64 extra block.
			// They should only be consulted if the sizes read earlier
			// are maxed out.
			// See golang.org/issue/13367.
			if needUSize {
				needUSize = false
				if len(fieldBuf) < 8 {
					return nil, zip.ErrFormat
				}
				entry.UncompressedSize64 = fieldBuf.uint64()
			}
			if needCSize {
				needCSize = false
				if len(fieldBuf) < 8 {
					return nil, zip.ErrFormat
				}
				entry.CompressedSize64 = fieldBuf.uint64()
			}
		}
	}

	if needCSize {
		return nil, zip.ErrFormat
	}

	if method == zip.Store {
		entry.r = io.LimitReader(z.r, int64(entry.UncompressedSize64))
	} else if method == zip.Deflate {
		var reader io.Reader
		if entry.CompressedSize64 > 0 {
			reader = io.LimitReader(z.r, int64(entry.CompressedSize64))
		} else {
			// unknown size; read until deflate EOF,
			// but we need z.r to be an io.ByteReader for flate to not overread
			if _, ok := z.r.(io.ByteReader); !ok {
				z.r = bufio.NewReader(z.r)
			}
			reader = z.r
		}
		if z.fr == nil {
			z.fr = flate.NewReader(reader)
		} else {
			z.fr.(flate.Resetter).Reset(reader, nil)
		}
		entry.r = z.fr
	} else {
		return nil, fmt.Errorf("unknown compression method %d", method)
	}

	return entry, nil
}

func (z *Reader) Next() (*Entry, error) {
	if z.localFileEnd {
		return nil, io.EOF
	}
	if z.curEntry != nil {
		// Read any remaining data for the current file, if necessary.
		if _, err := io.Copy(io.Discard, z.curEntry); err != nil {
			return nil, fmt.Errorf("read previous file data fail: %w", err)
		}
		// Read the data descriptor if present.
		if z.curEntry.hasDataDescriptor() {
			if err := readDataDescriptor(z.r); err != nil {
				return nil, fmt.Errorf("read previous entry's data descriptor fail: %w", err)
			}
		}
	}
	headerIDBuf := make([]byte, headerIdentifierLen)
	if _, err := io.ReadFull(z.r, headerIDBuf); err != nil {
		return nil, fmt.Errorf("unable to read header identifier: %w", err)
	}
	headerID := binary.LittleEndian.Uint32(headerIDBuf)
	if headerID != fileHeaderSignature {
		if headerID == directoryHeaderSignature || headerID == directoryEndSignature {
			z.localFileEnd = true
			return nil, io.EOF
		}
		return nil, zip.ErrFormat
	}
	entry, err := z.readEntry()
	if err != nil {
		return nil, fmt.Errorf("unable to read zip file header: %w", err)
	}
	z.curEntry = entry
	return entry, nil
}

func readDataDescriptor(r io.Reader) error {
	var buf [dataDescriptorLen]byte
	// The spec says: "Although not originally assigned a
	// signature, the value 0x08074b50 has commonly been adopted
	// as a signature value for the data descriptor record.
	// Implementers should be aware that ZIP files may be
	// encountered with or without this signature marking data
	// descriptors and should account for either case when reading
	// ZIP files to ensure compatibility."
	//
	// dataDescriptorLen includes the size of the signature but
	// first read just those 4 bytes to see if it exists.
	_, err := io.ReadFull(r, buf[:4])
	if err != nil {
		return err
	}
	off := 0
	maybeSig := readBuf(buf[:4])
	if maybeSig.uint32() != dataDescriptorSignature {
		// No data descriptor signature. Keep these four bytes.
		off += 4
	}
	_, err = io.ReadFull(r, buf[off:12])
	if err != nil {
		return err
	}

	return nil
}

type readBuf []byte

func (b *readBuf) uint8() uint8 {
	v := (*b)[0]
	*b = (*b)[1:]
	return v
}

func (b *readBuf) uint16() uint16 {
	v := binary.LittleEndian.Uint16(*b)
	*b = (*b)[2:]
	return v
}

func (b *readBuf) uint32() uint32 {
	v := binary.LittleEndian.Uint32(*b)
	*b = (*b)[4:]
	return v
}

func (b *readBuf) uint64() uint64 {
	v := binary.LittleEndian.Uint64(*b)
	*b = (*b)[8:]
	return v
}

func (b *readBuf) sub(n int) readBuf {
	b2 := (*b)[:n]
	*b = (*b)[n:]
	return b2
}
